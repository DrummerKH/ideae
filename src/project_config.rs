//! What a project does for each task besides starting its agent: commands
//! that make a new worktree ready (setup), that clean up after it
//! (teardown), one that tells whether the agent's work holds (check), the
//! ignored files a worktree needs, and its services.
//!
//! Kept in `.ideae.toml` at the repository root, shared with the team:
//!
//! ```toml
//! [task]
//! setup = ["npm ci"]
//! teardown = ["docker compose down"]
//! check = "npm test"
//! copy = [".env"]
//! ```
//!
//! or in the app's state, for yourself alone. That one, when there, is
//! used in place of the file, all of it. A worktree's own `.ideae.toml`
//! comes before the main checkout's, which new tasks fall back to.

use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Deserializer, Serialize};

use crate::services::{FILE, Service};

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(default)]
pub struct TaskSetup {
    #[serde(deserialize_with = "lines", skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    #[serde(deserialize_with = "lines", skip_serializing_if = "Vec::is_empty")]
    pub teardown: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub check: String,
    /// Ignored files copied from the main checkout into a new worktree, as
    /// git globs from the repository root; none for the `.env*` files, when
    /// Settings say to copy those.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy: Option<Vec<String>>,
}

impl TaskSetup {
    /// Commands that run without a click: what a repository's file must be
    /// trusted for.
    pub fn runs_anything(&self) -> bool {
        !self.setup.is_empty() || !self.teardown.is_empty() || !self.check.trim().is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub task: TaskSetup,
    pub service: Vec<Service>,
}

/// A command, or several: one a line in a string, or a list.
fn lines<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let all = match OneOrMany::deserialize(d)? {
        OneOrMany::One(text) => text.lines().map(str::to_string).collect(),
        OneOrMany::Many(list) => list,
    };
    Ok(all.into_iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect())
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| format!("{FILE}: {e}"))
}

/// The file in `dir`; none when there is no file.
pub fn read_file(dir: &Path) -> Result<Option<Config>, String> {
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(text) => parse(&text).map(Some),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------- what applies

/// The settings kept for yourself, by project; and the project each task's
/// worktree is of. Set by the window, read from any thread.
static OWN: LazyLock<RwLock<HashMap<PathBuf, Config>>> = LazyLock::new(Default::default);
static PROJECT_OF: LazyLock<RwLock<HashMap<PathBuf, PathBuf>>> = LazyLock::new(Default::default);

pub fn set_own(project: &Path, own: Option<Config>) {
    let mut all = OWN.write().unwrap();
    match own {
        Some(own) => all.insert(project.to_path_buf(), own),
        None => all.remove(project),
    };
}

pub fn own(project: &Path) -> Option<Config> {
    OWN.read().unwrap().get(project).cloned()
}

pub fn register(task_root: &Path, project: &Path) {
    PROJECT_OF.write().unwrap().insert(task_root.to_path_buf(), project.to_path_buf());
}

pub fn project_of(task_root: &Path) -> PathBuf {
    PROJECT_OF.read().unwrap().get(task_root).cloned().unwrap_or_else(|| task_root.to_path_buf())
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Resolved {
    pub config: Config,
    /// Out of a repository's `.ideae.toml`: someone else may have written
    /// it, so its commands run once trusted.
    pub from_file: bool,
    pub error: Option<String>,
}

/// What applies to the task at `root`: the settings kept for yourself,
/// else the worktree's file, else the main checkout's.
pub fn resolve(root: &Path) -> Resolved {
    let project = project_of(root);
    choose(own(&project), || read_file(root), || if project == root { Ok(None) } else { read_file(&project) })
}

fn choose(
    own: Option<Config>,
    worktree: impl FnOnce() -> Result<Option<Config>, String>,
    project: impl FnOnce() -> Result<Option<Config>, String>,
) -> Resolved {
    if let Some(config) = own {
        return Resolved { config, from_file: false, error: None };
    }
    let found = match worktree() {
        Ok(None) => project(),
        other => other,
    };
    match found {
        Ok(Some(config)) => Resolved { config, from_file: true, error: None },
        Ok(None) => Resolved::default(),
        Err(e) => Resolved { error: Some(e), ..Default::default() },
    }
}

/// What the commands of a file are known by when you allow them: a new
/// command in it asks again.
pub fn trust_key(setup: &TaskSetup) -> Option<String> {
    use sha2::Digest as _;
    if !setup.runs_anything() {
        return None;
    }
    let mut hash = sha2::Sha256::new();
    for (name, commands) in [("setup", setup.setup.join("\n")), ("teardown", setup.teardown.join("\n")), ("check", setup.check.trim().to_string())] {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(commands.as_bytes());
        hash.update([0]);
    }
    Some(hash.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------- writing

const HEADER: &str = "# What ideae does for each task of this repository (Project settings…).\n\
# setup runs in a new task's worktree, teardown when the task is closed or\n\
# deleted, check after each turn of its agent; copy names ignored files the\n\
# worktree gets from the main checkout. Services are started from Run.\n";

/// `text` (a `.ideae.toml`, or none) with `config` in it; what else it
/// says, comments included, stays.
pub fn render(text: Option<&str>, config: &Config) -> Result<String, String> {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};
    let mut doc: DocumentMut = text.unwrap_or("").parse().map_err(|e| format!("{FILE}: {e}"))?;
    let before = text.and_then(|t| parse(t).ok()).unwrap_or_default();
    let list = |items: &[String]| {
        let mut array = Array::new();
        for item in items {
            array.push(item.as_str());
        }
        value(array)
    };
    let t = &config.task;
    if *t != before.task || !doc.contains_key("task") {
        if !t.runs_anything() && t.copy.is_none() {
            doc.remove("task");
        } else {
            if !doc.get("task").is_some_and(Item::is_table) {
                // Before the services, where a reader looks first, under
                // the comment that opens the file.
                let mut table = Table::new();
                table.set_position(0);
                let opening = first_table(&mut doc).and_then(|first| {
                    let prefix = first.decor().prefix().and_then(|p| p.as_str()).map(str::to_string)?;
                    first.decor_mut().set_prefix("\n");
                    Some(prefix)
                });
                table.decor_mut().set_prefix(opening.unwrap_or_default());
                doc.insert("task", Item::Table(table));
            }
            let table = doc["task"].as_table_mut().expect("a table");
            let mut set = |key: &str, item: Option<Item>| match item {
                Some(item) => {
                    table.insert(key, item);
                }
                None => {
                    table.remove(key);
                }
            };
            set("setup", (!t.setup.is_empty()).then(|| list(&t.setup)));
            set("teardown", (!t.teardown.is_empty()).then(|| list(&t.teardown)));
            set("check", (!t.check.trim().is_empty()).then(|| value(t.check.trim())));
            set("copy", t.copy.as_ref().map(|c| list(c)));
        }
    }
    if config.service != before.service {
        doc.remove("service");
        if !config.service.is_empty() {
            let mut all = ArrayOfTables::new();
            for s in &config.service {
                let mut table = Table::new();
                table.insert("name", value(s.name.as_str()));
                table.insert("command", value(s.command.as_str()));
                if let Some(port) = s.port {
                    table.insert("port", value(i64::from(port)));
                }
                if let Some(url) = &s.url {
                    table.insert("url", value(url.as_str()));
                }
                if !s.env.is_empty() {
                    let mut env = InlineTable::new();
                    for (k, v) in &s.env {
                        env.insert(k, v.as_str().into());
                    }
                    table.insert("env", value(env));
                }
                if let Some(cwd) = &s.cwd {
                    table.insert("cwd", value(cwd.as_str()));
                }
                all.push(table);
            }
            doc.insert("service", Item::ArrayOfTables(all));
        }
    }
    let out = doc.to_string();
    Ok(if text.is_none() { format!("{HEADER}\n{}", out.trim_start()) } else { out })
}

/// The table that comes first in the file, which its opening comment is of.
fn first_table(doc: &mut toml_edit::DocumentMut) -> Option<&mut toml_edit::Table> {
    use toml_edit::Item;
    doc.as_table_mut()
        .iter_mut()
        .filter_map(|(_, item)| match item {
            Item::Table(t) => Some(t),
            Item::ArrayOfTables(a) => a.iter_mut().next(),
            _ => None,
        })
        .min_by_key(|t| t.position().unwrap_or(usize::MAX))
}

/// Writes `config` into the `.ideae.toml` in `dir`.
pub fn write_file(dir: &Path, config: &Config) -> Result<(), String> {
    let path = dir.join(FILE);
    let text = std::fs::read_to_string(&path).ok();
    let out = render(text.as_deref(), config)?;
    std::fs::write(&path, out).map_err(|e| format!("{FILE}: {e}"))
}

// ---------------------------------------------------------------- suggestions

/// What the project's own files say it needs, offered, never put in by itself.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Suggested {
    pub setup: Vec<String>,
    pub teardown: Vec<String>,
    pub check: String,
    pub copy: Vec<String>,
    pub services: Vec<Service>,
}

impl Suggested {
    pub fn is_empty(&self) -> bool {
        self.setup.is_empty() && self.teardown.is_empty() && self.check.is_empty() && self.copy.is_empty() && self.services.is_empty()
    }
}

pub fn suggest(root: &Path) -> Suggested {
    let has = |name: &str| root.join(name).exists();
    let mut out = Suggested::default();
    let scripts = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|json| json["scripts"].clone());
    // npm's placeholder for a project without tests is no check.
    let tests = scripts.as_ref().and_then(|s| s["test"].as_str()).is_some_and(|t| !t.contains("no test specified"));
    let node = [
        ("pnpm-lock.yaml", "pnpm install --frozen-lockfile", "pnpm test"),
        ("bun.lock", "bun install --frozen-lockfile", "bun run test"),
        ("bun.lockb", "bun install --frozen-lockfile", "bun run test"),
        ("yarn.lock", if has(".yarnrc.yml") { "yarn install --immutable" } else { "yarn install --frozen-lockfile" }, "yarn test"),
        ("package-lock.json", "npm ci", "npm test"),
    ];
    if let Some((_, install, test)) = node.into_iter().find(|(lock, _, _)| has(lock)) {
        out.setup.push(install.into());
        if tests {
            out.check = test.into();
        }
    } else if scripts.is_some() {
        out.setup.push("npm install".into());
        if tests {
            out.check = "npm test".into();
        }
    }
    if has("Cargo.toml") && out.check.is_empty() {
        out.check = "cargo test".into();
    }
    if has("go.mod") {
        out.setup.push("go mod download".into());
        if out.check.is_empty() {
            out.check = "go test ./...".into();
        }
    }
    if has("gradlew") {
        if out.check.is_empty() {
            out.check = "./gradlew test".into();
        }
    } else if has("mvnw") && out.check.is_empty() {
        out.check = "./mvnw -q test".into();
    }
    if has("uv.lock") {
        out.setup.push("uv sync".into());
        if out.check.is_empty() {
            out.check = "uv run pytest".into();
        }
    } else if has("poetry.lock") {
        out.setup.push("poetry install".into());
        if out.check.is_empty() {
            out.check = "poetry run pytest".into();
        }
    } else if has("requirements.txt") || has("pyproject.toml") {
        let install = if has("requirements.txt") { ".venv/bin/pip install -r requirements.txt" } else { ".venv/bin/pip install -e ." };
        out.setup.push(format!("python3 -m venv .venv && {install}"));
        if out.check.is_empty() {
            out.check = ".venv/bin/python -m pytest".into();
        }
    }
    if has("Gemfile.lock") {
        out.setup.push("bundle install".into());
    }
    if has("composer.lock") {
        out.setup.push("composer install".into());
    }
    if has(".env.example") || (has(".env") && ignored(root, ".env")) {
        out.copy.push(".env".into());
    }
    if ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"].iter().any(|f| has(f)) {
        out.teardown.push("docker compose down".into());
    }
    out.services = crate::services::detect(root).into_iter().map(|f| f.service).collect();
    out
}

fn ignored(root: &Path, file: &str) -> bool {
    crate::procs::command("git").current_dir(root).args(["check-ignore", "-q", file]).status().is_ok_and(|s| s.success())
}

/// Ignored files of the main checkout matching `patterns`, copied into the
/// worktree at `to` where it has none of its own.
pub fn copy_ignored(from: &Path, to: &Path, patterns: &[String]) {
    if patterns.is_empty() {
        return;
    }
    let mut args = vec!["ls-files".to_string(), "--others".into(), "--ignored".into(), "--exclude-standard".into(), "-z".into(), "--".into()];
    args.extend(patterns.iter().map(|p| format!(":(glob){p}")));
    let Ok(out) = crate::procs::command("git").current_dir(from).args(&args).output() else { return };
    for rel in String::from_utf8_lossy(&out.stdout).split('\0').filter(|f| !f.is_empty()) {
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

// ---------------------------------------------------------------- running

/// How a run of commands went.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub code: Option<i32>,
    /// The end of what they printed, escapes taken out.
    pub output: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }
}

/// What is kept of a run's output.
const KEEP: usize = 64 * 1024;

/// One script of `commands`: each runs in turn, the first that fails ends it.
fn script(commands: &[String]) -> String {
    if cfg!(windows) {
        commands.iter().map(|c| format!("({c})")).collect::<Vec<_>>().join(" && ")
    } else {
        commands.iter().map(|c| format!("{{ {c}\n}} || exit $?\n")).collect()
    }
}

/// Runs `commands` in `cwd` and waits for them, `timeout` at most, or
/// until `cancel` is set. They run in a session of their own, which is
/// ended whole: what they start goes with them, and with the app.
pub fn run(commands: &[String], cwd: &Path, env: &[(String, String)], timeout: Duration, cancel: &AtomicBool) -> Outcome {
    let mut command = if cfg!(windows) {
        let mut c = crate::procs::command("cmd");
        c.arg("/C").arg(format!("({}) 2>&1", script(commands)));
        c
    } else {
        let mut c = crate::procs::command("sh");
        c.arg("-c").arg(format!("exec 2>&1\n{}", script(commands)));
        c
    };
    command.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v))).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid only changes the child's own session.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = match crate::procs::spawn_tied(&mut command) {
        Ok(child) => child,
        Err(e) => return Outcome { output: e.to_string(), ..Default::default() },
    };
    let kept = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader = child.stdout.take().map(|mut out| {
        let kept = kept.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut kept = kept.lock().unwrap();
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * KEEP {
                    let cut = kept.len() - KEEP;
                    kept.drain(..cut);
                }
            }
        })
    });
    let started = Instant::now();
    let (mut timed_out, mut cancelled) = (false, false);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => break None,
        }
        timed_out = started.elapsed() > timeout;
        cancelled = cancel.load(Ordering::Relaxed);
        if timed_out || cancelled {
            end(&mut child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A program it left running holds the pipe: what came so far is enough.
    if let Some(reader) = reader {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let bytes = kept.lock().unwrap().clone();
    let start = bytes.len().saturating_sub(KEEP);
    Outcome { code, output: plain(&bytes[start..]), timed_out, cancelled }
}

/// Ends the commands' whole session: a term first, then a kill.
fn end(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = child.id() as i32;
        // SAFETY: kill only sends a signal, here to the run's own group.
        unsafe { libc::kill(-group, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: as above.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        let _ = crate::procs::command("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Output as text to read: colors and cursor moves out, a line redrawn
/// with \r (a progress bar) as it was last drawn.
pub fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut line = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => line.clear(),
            '\n' => {
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            c if c.is_control() && c != '\t' => {}
            c => line.push(c),
        }
    }
    out.push_str(&line);
    out
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// What the worktree holds now, as one value: a check of the same is not
/// run again. None outside a repository.
pub fn fingerprint(root: &Path) -> Option<String> {
    use sha2::Digest as _;
    let git = |args: &[&str]| crate::procs::command("git").current_dir(root).args(args).output().ok().filter(|o| o.status.success()).map(|o| o.stdout);
    let mut hash = sha2::Sha256::new();
    hash.update(git(&["rev-parse", "HEAD"])?);
    let status = git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    hash.update(&status);
    hash.update(git(&["diff", "HEAD", "--binary"]).unwrap_or_default());
    // New files are in no diff: their size and time stand for them.
    for entry in status.split(|&b| b == 0) {
        if let Some(rel) = entry.strip_prefix(b"?? ") {
            let path = root.join(String::from_utf8_lossy(rel).as_ref());
            if let Ok(meta) = std::fs::metadata(&path) {
                hash.update(rel);
                hash.update(meta.len().to_le_bytes());
                if let Ok(at) = meta.modified().map(|m| m.duration_since(std::time::UNIX_EPOCH).unwrap_or_default()) {
                    hash.update(at.as_nanos().to_le_bytes());
                }
            }
        }
    }
    Some(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The script a setup terminal runs: the commands in turn, each shown
/// first, the first that fails ending it; then its exit code into `status`.
/// Its name ends as the shell it is for wants.
pub fn setup_script(commands: &[String], status: &Path) -> String {
    let status = status.display().to_string();
    if cfg!(windows) {
        let mut out = String::from("@echo off\r\n");
        for c in commands {
            out.push_str(&format!("echo $ {c}\r\ncall {c} || goto failed\r\n"));
        }
        out.push_str(&format!(
            "(echo 0)> \"{status}\"\r\necho.\r\necho Setup done\r\nexit /b 0\r\n:failed\r\n(echo 1)> \"{status}\"\r\necho.\r\necho Setup failed\r\nexit /b 1\r\n"
        ));
        return out;
    }
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut out = String::from("(\n");
    for c in commands {
        out.push_str(&format!("printf '\\033[2m$ %s\\033[0m\\n' {}\n{{ {c}\n}} || exit $?\n", quote(c)));
    }
    out.push_str(&format!(
        ")\ns=$?\nprintf '%s' \"$s\" > {}\nif [ \"$s\" = 0 ]; then printf '\\n\\033[32m✓ Setup done\\033[0m\\n'; else printf '\\n\\033[31m✗ Setup failed (exit %s)\\033[0m\\n' \"$s\"; fi\n",
        quote(&status)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ideae-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn suggests_from_the_files_a_project_has() {
        let root = scratch("suggest");
        std::fs::write(root.join("package.json"), r#"{"scripts":{"dev":"vite","test":"vitest"}}"#).unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(root.join(".env.example"), "A=1").unwrap();
        let s = suggest(&root);
        assert_eq!(s.setup, ["pnpm install --frozen-lockfile"]);
        assert_eq!(s.check, "pnpm test");
        assert_eq!(s.copy, [".env"]);
        assert_eq!(s.services.first().map(|s| s.name.as_str()), Some("dev"));

        let root = scratch("suggest-rust-go");
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("go.mod"), "").unwrap();
        let s = suggest(&root);
        assert_eq!(s.check, "cargo test", "the first tool found gives the check");
        assert_eq!(s.setup, ["go mod download"]);
        assert!(s.copy.is_empty());

        let root = scratch("suggest-npm");
        std::fs::write(root.join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#).unwrap();
        std::fs::write(root.join("package-lock.json"), "{}").unwrap();
        std::fs::write(root.join("gradlew"), "").unwrap();
        let s = suggest(&root);
        assert_eq!(s.setup, ["npm ci"]);
        assert_eq!(s.check, "./gradlew test", "npm's placeholder is no test");
        assert!(suggest(&scratch("suggest-empty")).is_empty());
        for name in ["suggest", "suggest-rust-go", "suggest-npm", "suggest-empty"] {
            let _ = std::fs::remove_dir_all(scratch(name));
        }
    }

    #[test]
    fn the_file_is_written_and_read_back_keeping_the_rest() {
        let text = "# ours\n[[service]]\nname = \"web\" # the site\ncommand = \"npm run dev\"\nport = 3000\n";
        let mut config = parse(text).unwrap();
        config.task = TaskSetup {
            setup: vec!["npm ci".into(), "make gen".into()],
            teardown: vec!["docker compose down".into()],
            check: "npm test".into(),
            copy: Some(vec![".env".into()]),
        };
        let out = render(Some(text), &config).unwrap();
        assert!(out.contains("# ours") && out.contains("# the site"), "comments stay:\n{out}");
        assert!(out.find("[task]") < out.find("[[service]]"), "the task's part first:\n{out}");
        assert_eq!(parse(&out).unwrap(), config);

        config.service.push(Service { name: "api".into(), command: "make api".into(), port: Some(8080), url: None, env: Default::default(), cwd: Some("api".into()) });
        config.task = TaskSetup::default();
        let out = render(Some(&out), &config).unwrap();
        assert!(!out.contains("[task]"), "nothing left of it:\n{out}");
        assert_eq!(parse(&out).unwrap(), config);

        let fresh = render(None, &config).unwrap();
        assert!(fresh.starts_with("# What ideae does"));
        assert_eq!(parse(&fresh).unwrap(), config);
        // One string of lines reads as a list.
        let lines = parse("[task]\nsetup = \"npm ci\\n\\nmake\"\n").unwrap();
        assert_eq!(lines.task.setup, ["npm ci", "make"]);
    }

    #[test]
    fn trust_follows_the_commands() {
        let a = TaskSetup { setup: vec!["npm ci".into()], ..Default::default() };
        let mut b = a.clone();
        b.copy = Some(vec![".env".into()]);
        assert_eq!(trust_key(&a), trust_key(&b), "files to copy run nothing");
        b.check = "npm test".into();
        assert_ne!(trust_key(&a), trust_key(&b));
        let moved = TaskSetup { teardown: vec!["npm ci".into()], ..Default::default() };
        assert_ne!(trust_key(&a), trust_key(&moved), "the same command elsewhere is another");
        assert_eq!(trust_key(&TaskSetup::default()), None);
        assert_eq!(trust_key(&a).map(|k| k.len()), Some(32));
    }

    #[test]
    fn your_own_settings_come_before_the_file() {
        let file = Config { task: TaskSetup { check: "make test".into(), ..Default::default() }, ..Default::default() };
        let own = Config { task: TaskSetup { check: "make quick".into(), ..Default::default() }, ..Default::default() };
        let r = choose(Some(own.clone()), || Ok(Some(file.clone())), || unreachable!());
        assert_eq!((r.config, r.from_file), (own, false));
        let r = choose(None, || Ok(None), || Ok(Some(file.clone())));
        assert_eq!((r.config.task.check.as_str(), r.from_file), ("make test", true), "the main checkout's file when the worktree has none");
        let r = choose(None, || Err("bad".into()), || Ok(Some(file.clone())));
        assert_eq!(r.error.as_deref(), Some("bad"));
        assert_eq!(choose(None, || Ok(None), || Ok(None)), Resolved::default());

        let (project, task) = (scratch("own-project"), scratch("own-task"));
        std::fs::write(project.join(FILE), "[task]\ncheck = \"make test\"\n").unwrap();
        register(&task, &project);
        assert_eq!(resolve(&task).config.task.check, "make test");
        set_own(&project, Some(Config::default()));
        assert!(!resolve(&task).from_file);
        set_own(&project, None);
        let _ = (std::fs::remove_dir_all(project), std::fs::remove_dir_all(task));
    }

    #[test]
    fn runs_bounded_and_says_how_it_went() {
        let dir = scratch("run");
        let cancel = AtomicBool::new(false);
        let ok = run(&["echo one".into(), "printf '\\033[31mred\\033[0m\\n' >&2".into()], &dir, &[], Duration::from_secs(10), &cancel);
        assert!(ok.ok(), "{ok:?}");
        assert_eq!(ok.output, "one\nred\n");
        let failed = run(&["echo a".into(), "exit 3".into(), "echo never".into()], &dir, &[("X".into(), "y".into())], Duration::from_secs(10), &cancel);
        assert_eq!((failed.code, failed.output.as_str()), (Some(3), "a\n"));
        let slow = run(&["sleep 5".into()], &dir, &[], Duration::from_millis(200), &cancel);
        assert!(slow.timed_out && !slow.ok());
        assert_eq!(tail("1\n2\n3\n", 2), "2\n3");
        assert_eq!(plain(b"10%\r50%\r100%\ndone"), "100%\ndone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_setup_script_tells_how_it_ended() {
        if cfg!(windows) {
            return;
        }
        let dir = scratch("script");
        let status = dir.join("status");
        let body = setup_script(&["echo 'it''s'".into(), "false".into(), "echo never".into()], &status);
        std::fs::write(dir.join("setup.sh"), body).unwrap();
        let out = std::process::Command::new("sh").arg(dir.join("setup.sh")).current_dir(&dir).output().unwrap();
        assert_eq!(std::fs::read_to_string(&status).unwrap(), "1");
        let text = plain(&out.stdout);
        assert!(text.contains("its") && text.contains("Setup failed") && !text.contains("never"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
