//! One language server for the tasks of a project: a worktree is one more
//! folder of the server that runs, not one more server. What every worktree
//! has alike (the standard library, the dependencies, the system's headers)
//! is read once: three tasks of one Rust project take about a third of the
//! memory of three servers. Measured for the servers listed in [`share`];
//! others say they take folders added later and do not read them
//! (kotlin-lsp, intelephense), or mix the worktrees up (jdtls).
//!
//! A task holds its folder with a [`Lease`]: let go (asleep, paused,
//! archived, closed), the folder leaves the server, and the last one ends it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use anyhow::Result;
use serde_json::Value;

use crate::lsp::LspClient;

/// How a server takes one more worktree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Share {
    /// Told with `workspace/didChangeWorkspaceFolders`.
    Folders,
    /// Finds a file's project by itself (tsconfig, compile_commands.json):
    /// opening the file is enough.
    Files,
}

/// The servers that serve several worktrees well, and how.
pub fn share(program: &str) -> Option<Share> {
    match program {
        "rust-analyzer" | "gopls" | "basedpyright-langserver" => Some(Share::Folders),
        "typescript-language-server" | "clangd" => Some(Share::Files),
        _ => None,
    }
}

/// rust-analyzer keeps what it read of a folder after letting go of it,
/// about 20 MB each: after this many it is started anew.
const STALE_AFTER: usize = 8;

/// What tasks have to have alike to share a server.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    program: &'static str,
    /// The repository's main checkout.
    home: PathBuf,
    /// The project's directory in the worktree: `.` or `backend`.
    within: PathBuf,
    /// What picks the server's own version or the toolchain it reads with:
    /// a branch with another one gets a server of its own.
    version: String,
}

static SHARED: Mutex<Option<HashMap<Key, Weak<LspClient>>>> = Mutex::new(None);

/// A task's hold on a server: its folder in it. Dropped, the folder goes,
/// and with the last one the server.
pub struct Lease {
    client: Arc<LspClient>,
    folder: PathBuf,
}

impl Lease {
    pub fn client(&self) -> &Arc<LspClient> {
        &self.client
    }
}

impl std::ops::Deref for Lease {
    type Target = Arc<LspClient>;
    fn deref(&self) -> &Arc<LspClient> {
        &self.client
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.client.remove_folder(&self.folder) {
            self.client.shut_down();
        }
    }
}

/// The server for `folder`, a project in the worktree `root`: the one the
/// project's other tasks have, given one more folder, or a new one.
pub fn lease(program: &'static str, command: &Path, args: &[&str], folder: &Path, root: &Path, init_options: Value) -> Result<Lease> {
    let key = share(program)
        .filter(|_| !crate::state::with_settings(|s| s.lsp_separate))
        .and_then(|_| key(program, folder, root));
    let Some(key) = key else {
        let client = LspClient::spawn(program, command, args, folder, init_options)?;
        return Ok(Lease { client, folder: folder.to_path_buf() });
    };
    let mut shared = SHARED.lock().unwrap();
    let shared = shared.get_or_insert_with(HashMap::new);
    shared.retain(|_, client| client.strong_count() > 0);
    if let Some(client) = shared.get(&key).and_then(Weak::upgrade).filter(|c| !c.is_gone() && !stale(c)) {
        client.add_folder(folder);
        return Ok(Lease { client, folder: folder.to_path_buf() });
    }
    let client = LspClient::spawn_in(program, command, args, folder, &key.home, share(program), init_options)?;
    shared.insert(key, Arc::downgrade(&client));
    Ok(Lease { client, folder: folder.to_path_buf() })
}

/// A task's servers, one per (program, project directory): held by the
/// task, found and started off the UI thread. Started one at a time, so two
/// files of a language opened at once have one server; a generation, so
/// one started while the task stopped its servers is let go of, not kept.
#[derive(Default)]
pub struct TaskServers {
    leases: Mutex<HashMap<(&'static str, PathBuf), Lease>>,
    generation: std::sync::atomic::AtomicU64,
    starting: Mutex<()>,
}

/// A file's server, as [`TaskServers::server_for`] found it.
pub struct Found {
    pub client: Arc<LspClient>,
    pub language_id: &'static str,
    pub program: &'static str,
    pub project: PathBuf,
}

impl TaskServers {
    pub fn clients(&self) -> Vec<Arc<LspClient>> {
        self.leases.lock().unwrap().values().map(|l| l.client().clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.lock().unwrap().is_empty()
    }

    pub fn holds(&self, client: &Arc<LspClient>) -> bool {
        self.leases.lock().unwrap().values().any(|l| Arc::ptr_eq(l.client(), client))
    }

    /// Each by its key, for a look at them all.
    pub fn entries(&self) -> Vec<((&'static str, PathBuf), Arc<LspClient>)> {
        self.leases.lock().unwrap().iter().map(|(k, l)| (k.clone(), l.client().clone())).collect()
    }

    /// Lets go of one: its folder leaves the server, or the server ends.
    pub fn remove(&self, key: &(&'static str, PathBuf)) {
        let lease = self.leases.lock().unwrap().remove(key);
        drop(lease);
    }

    /// Lets go of them all, and of those being started now.
    pub fn clear(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let all = std::mem::take(&mut *self.leases.lock().unwrap());
        drop(all);
    }

    /// The server for a file of the task at `root`, started (or its folder
    /// added to the project's shared one) when the task has none yet; or
    /// why there is none. Blocks: reads files, starts a process.
    pub fn server_for(&self, root: &Path, path: &Path) -> Result<Found, String> {
        let crate::lsp::Server { language_id, program, command, args } = crate::lsp::server_for(path)?;
        let generation = self.generation.load(std::sync::atomic::Ordering::SeqCst);
        let _one = self.starting.lock().unwrap();
        let project = if path.starts_with(root) {
            crate::lsp::project_root(program, path, root)
        } else {
            // A library file (std, the registry, a jump target): the task's own
            // server for the language reads it; one more would index it all again.
            let known = self.leases.lock().unwrap().keys().find(|(p, _)| *p == program).map(|(_, project)| project.clone());
            known.unwrap_or_else(|| crate::lsp::project_root(program, &root.join("_"), root))
        };
        let key = (program, project.clone());
        if let Some(lease) = self.leases.lock().unwrap().get(&key) {
            return Ok(Found { client: lease.client().clone(), language_id, program, project });
        }
        let lease = lease(program, &command, args, &project, root, crate::lsp_install::init_options(program, &command)).map_err(|error| {
            // Not installed is the normal case for many languages, not an error.
            let missing = error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            if missing { format!("{program} not installed") } else { format!("{program} failed: {error:#}") }
        })?;
        let client = lease.client().clone();
        let mut leases = self.leases.lock().unwrap();
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
            drop(leases);
            drop(lease);
            return Err(STOPPED.into());
        }
        leases.insert(key, lease);
        Ok(Found { client, language_id, program, project })
    }
}

/// Why a server started for a task was let go of at once.
pub const STOPPED: &str = "language servers stopped meanwhile";

/// A shared server to start anew: it has let go of so many folders that
/// what it kept of them weighs. Given to no new task, it ends when the
/// last that holds it lets go (asleep out of view), and what that task
/// opens next has a new one: nothing is moved while it is looked at.
pub fn stale(client: &LspClient) -> bool {
    client.sharing().is_some() && client.removed() >= STALE_AFTER
}

/// Whether a server that ended by itself is started again: three of a
/// program in ten minutes, then it is left as it is, its failure shown.
/// Asked by every task that held it; one ending counts once.
pub fn may_restart(program: &str, ended: &Arc<LspClient>) -> bool {
    static RESTARTS: Mutex<Option<HashMap<String, Vec<(Instant, usize)>>>> = Mutex::new(None);
    let id = Arc::as_ptr(ended) as usize;
    let mut restarts = RESTARTS.lock().unwrap();
    let times = restarts.get_or_insert_with(HashMap::new).entry(program.to_string()).or_default();
    let now = Instant::now();
    times.retain(|(at, _)| now.duration_since(*at) < Duration::from_secs(600));
    if times.iter().any(|(_, which)| *which == id) {
        return true;
    }
    if times.len() >= 3 {
        return false;
    }
    times.push((now, id));
    true
}

fn key(program: &'static str, folder: &Path, root: &Path) -> Option<Key> {
    let home = main_checkout(root)?;
    let within = folder.strip_prefix(root).ok()?.to_path_buf();
    Some(Key { program, home, within, version: version(program, folder, root) })
}

/// The main checkout of the repository `root` is a worktree of: `root`
/// itself when it is the main one. A file read, no git. Without links:
/// git writes a worktree's gitdir as the real path (`/private/var` for
/// `/var` on macOS).
fn main_checkout(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return std::fs::canonicalize(root).ok();
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() { gitdir } else { root.join(gitdir) };
    // <main>/.git/worktrees/<name>
    let common = gitdir.parent()?.parent()?;
    (common.file_name()? == ".git").then(|| common.parent().and_then(|main| std::fs::canonicalize(main).ok())).flatten()
}

fn version(program: &str, folder: &Path, root: &Path) -> String {
    match program {
        // rustup picks the toolchain, and with it rust-analyzer's sysroot,
        // from the nearest file naming one.
        "rust-analyzer" => folder
            .ancestors()
            .take_while(|dir| dir.starts_with(root))
            .find_map(|dir| {
                ["rust-toolchain.toml", "rust-toolchain"].iter().find_map(|name| std::fs::read_to_string(dir.join(name)).ok())
            })
            .unwrap_or_default(),
        // The server runs the project's own TypeScript, from the folder it
        // was started for.
        "typescript-language-server" => std::fs::read(folder.join("node_modules/typescript/package.json"))
            .ok()
            .and_then(|text| serde_json::from_slice::<Value>(&text).ok())
            .and_then(|package| package["version"].as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ideae-share-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn worktrees_of_one_repository_have_its_main_checkout() {
        let dir = scratch("main");
        let main = dir.join("app");
        std::fs::create_dir_all(main.join(".git/worktrees/fix")).unwrap();
        let task = dir.join("app.worktrees/fix");
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/fix").display())).unwrap();
        let real = std::fs::canonicalize(&main).unwrap();
        assert_eq!(main_checkout(&main), Some(real.clone()));
        assert_eq!(main_checkout(&task), Some(real));
        assert_eq!(main_checkout(&dir.join("elsewhere")), None);
        let (a, b) = (key("rust-analyzer", &main, &main).unwrap(), key("rust-analyzer", &task, &task).unwrap());
        assert_eq!(a, b);
        // A branch on another toolchain has its own.
        std::fs::write(task.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.80\"\n").unwrap();
        assert_ne!(a, key("rust-analyzer", &task, &task).unwrap());
        // Another project of the worktree, another server.
        std::fs::create_dir_all(main.join("web")).unwrap();
        assert_ne!(a.within, key("rust-analyzer", &main.join("web"), &main).unwrap().within);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn wait_for<T>(what: &str, mut ask: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(found) = ask() {
                return found;
            }
            assert!(Instant::now() < until, "{what}: not in 90 s");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Two tasks of a Cargo project, the main checkout and a worktree: one
    /// rust-analyzer, each task's jumps in its own files, a folder let go
    /// of leaving the server, the last ending it. Passes without running
    /// where there is no rust-analyzer.
    #[test]
    #[ignore]
    fn two_tasks_share_one_rust_analyzer() {
        let dir = scratch("ra");
        let main = dir.join("demo");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(main.join("src/lib.rs"), "pub fn answer() -> u32 {\n    42\n}\n\npub fn ask() -> u32 {\n    answer()\n}\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "demo"]);
        let other = dir.join("demo.worktrees/two");
        git(&main, ["worktree", "add", "-q", "-b", "two"].iter().copied().chain([other.to_str().unwrap()]).collect::<Vec<_>>().as_slice());
        let file = |root: &Path| root.join("src/lib.rs");
        let Ok(server) = crate::lsp::server_for(&file(&main)) else {
            eprintln!("no rust-analyzer here: skipped");
            return;
        };
        let (lease_main, lease_other) = (
            lease(server.program, &server.command, server.args, &main, &main, Value::Null).unwrap(),
            lease(server.program, &server.command, server.args, &other, &other, Value::Null).unwrap(),
        );
        let client = lease_main.client().clone();
        assert!(Arc::ptr_eq(&client, lease_other.client()), "one server for both");
        assert_eq!(client.folders(), vec![main.clone(), other.clone()]);
        for root in [&main, &other] {
            client.did_open(&file(root), "rust", &std::fs::read_to_string(file(root)).unwrap()).unwrap();
        }
        // `answer()` in `ask`: each task's jump stays in its own worktree.
        let at = lsp_types::Position { line: 5, character: 6 };
        for root in [&main, &other] {
            let uri = crate::lsp::path_to_uri(&file(root));
            let target = wait_for("definition", || client.definition(&uri, at).ok()?.into_iter().next());
            assert_eq!(crate::lsp::uri_to_path(target.target_uri.as_str()), Some(file(root)), "from {}", root.display());
        }
        drop(lease_other);
        assert_eq!(client.folders(), vec![main.clone()]);
        // Its files closed with it, to be opened anew with their text then.
        assert!(!client.has_open(&file(&other)) && client.has_open(&file(&main)));
        assert!(!client.is_gone());
        drop(lease_main);
        wait_for("the server's end", || client.is_gone().then_some(()));
        // A task's two files asking at once, off the UI thread: one server.
        let servers = Arc::new(TaskServers::default());
        let asking: Vec<_> = (0..2)
            .map(|_| {
                let (servers, main) = (servers.clone(), main.clone());
                std::thread::spawn(move || servers.server_for(&main, &main.join("src/lib.rs")).map(|found| found.client))
            })
            .collect();
        let found: Vec<_> = asking.into_iter().map(|t| t.join().unwrap().unwrap()).collect();
        assert!(Arc::ptr_eq(&found[0], &found[1]) && servers.holds(&found[0]));
        servers.clear();
        assert!(servers.is_empty());
        wait_for("its end once let go of", || found[0].is_gone().then_some(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_servers_measured_are_shared() {
        assert_eq!(share("rust-analyzer"), Some(Share::Folders));
        assert_eq!(share("clangd"), Some(Share::Files));
        for program in ["kotlin-lsp", "intelephense", "jdtls", "phpactor"] {
            assert_eq!(share(program), None, "{program}");
        }
    }
}
