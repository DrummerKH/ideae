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
    pub active_task: Option<PathBuf>,
    /// Text size per kind of view.
    pub zoom: crate::zoom::Zoom,
    pub projects: Vec<ProjectState>,
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
