//! What survives a restart: projects, their tasks and each task's tabs, kept
//! in `~/.local/share/ideae/state.json` (`%LOCALAPPDATA%\ideae` on Windows).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct State {
    pub appearance: Option<String>,
    pub sidebar_width: Option<f32>,
    pub side_panel_width: Option<f32>,
    pub active_collapsed: bool,
    pub power_saving: bool,
    pub window: Option<WindowPlace>,
    pub settings: Settings,
    pub active_task: Option<PathBuf>,
    /// Text size per kind of view.
    pub zoom: crate::zoom::Zoom,
    pub projects: Vec<ProjectState>,
}

/// What the user set in Settings (Ctrl+,).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// The agent tab's command; empty for a plain shell. `--agent` and
    /// `--no-agent` on the command line win.
    pub agent: String,
    /// Language servers (program names) never started.
    pub lsp_off: Vec<String>,
    /// Minutes a task out of view keeps its language servers.
    pub lsp_idle_minutes: u32,
    pub notifications: bool,
    /// Copy the main checkout's ignored `.env*` files into new worktrees.
    pub copy_env: bool,
    pub pr_status: bool,
    /// Release builds ask GitHub for newer releases.
    pub check_updates: bool,
    /// Agents keep Claude Code's LSP plugins (a rust-analyzer per agent…).
    pub agent_lsp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agent: "claude".into(),
            lsp_off: Vec::new(),
            lsp_idle_minutes: 15,
            notifications: true,
            copy_env: true,
            pr_status: true,
            check_updates: true,
            agent_lsp: true,
        }
    }
}

static SETTINGS: std::sync::RwLock<Option<Settings>> = std::sync::RwLock::new(None);

/// The settings in force, readable from any thread.
pub fn settings() -> Settings {
    if let Some(s) = SETTINGS.read().unwrap().as_ref() {
        return s.clone();
    }
    let loaded = load().settings;
    *SETTINGS.write().unwrap() = Some(loaded.clone());
    loaded
}

pub fn set_settings(settings: Settings) {
    *SETTINGS.write().unwrap() = Some(settings);
}

/// The window's size and place. Wayland lets no app choose its position;
/// there the size and the maximized state still come back.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct WindowPlace {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    pub fullscreen: bool,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct ProjectState {
    /// The main worktree.
    pub path: PathBuf,
    pub collapsed: bool,
    pub tasks: Vec<TaskState>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct TaskState {
    /// The worktree; for the main thread, the project's own path.
    pub path: PathBuf,
    pub branch: String,
    pub title: String,
    pub main: bool,
    pub starred: bool,
    /// Unix seconds of the last time it was in view.
    pub last_active: u64,
    /// Running when the app quit, so started again with it.
    pub open: bool,
    /// Its share of the services' ports: 0 for the main checkout.
    pub port_slot: u32,
    pub session: TaskSession,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct TaskSession {
    pub tabs: Vec<TabSession>,
    /// Indices into `tabs`.
    pub left_active: Option<usize>,
    pub right_active: Option<usize>,
    pub focus_right: bool,
    /// Folders open in the file tree.
    pub expanded: Vec<String>,
    /// 0 Changes, 1 Files, 2 Structure.
    pub side_panel: u8,
    /// Review comments not sent to the agent yet.
    pub review: Vec<ReviewNote>,
    /// Changed files marked viewed, with a hash of their diff then.
    pub viewed: std::collections::BTreeMap<String, u64>,
}

/// A review comment on lines of the diff.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ReviewNote {
    /// "path:12-14".
    pub location: String,
    pub code: String,
    pub language: String,
    pub text: String,
    /// Where it shows in the diff: under this line of the file.
    pub path: String,
    pub modified_side: bool,
    pub line: usize,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct TabSession {
    /// agent, shell, changes, history, file, browser.
    pub kind: String,
    /// A file, relative to the worktree.
    pub path: Option<String>,
    /// 0-based cursor line of a file.
    pub line: u32,
    pub url: Option<String>,
    pub right: bool,
}

/// Debug builds are a separate app: their own data, sockets, window class
/// and launcher, so a dev build never touches the release one's tasks.
pub const APP_ID: &str = if cfg!(debug_assertions) { "ideae-dev" } else { "ideae" };

/// Where apps keep their data: `~/.local/share`, `%LOCALAPPDATA%` on Windows.
pub fn data_home() -> PathBuf {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return var("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = var("HOME") {
            return home.join("Library/Application Support");
        }
    }
    var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share"))).unwrap_or_else(std::env::temp_dir)
}

/// Sockets, hook files and browser profiles of running instances.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(APP_ID)
}

pub fn data_dir() -> PathBuf {
    data_home().join(APP_ID)
}

fn state_path() -> PathBuf {
    // A separate state for experiments, such as headless test runs.
    match std::env::var_os("IDEAE_STATE") {
        Some(path) => PathBuf::from(path),
        None => data_dir().join("state.json"),
    }
}

pub fn load() -> State {
    let path = state_path();
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            // Keep the unreadable file for a look rather than overwrite it.
            eprintln!("state {}: {e}; starting empty", path.display());
            let _ = std::fs::rename(&path, path.with_extension("json.bad"));
            State::default()
        }),
        Err(_) => State::default(),
    }
}

/// Writes the serialized state; a crash mid-write leaves the old file whole.
pub fn save(json: &str) -> std::io::Result<()> {
    write_atomic(&state_path(), json.as_bytes())
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
