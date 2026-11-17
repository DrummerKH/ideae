//! Worktrees an agent made for itself: Claude Code's subagents in
//! `.claude/worktrees/`, or one made in a temporary place to try something.
//! They are not tasks of the user's: listed folded under the task they came
//! from (or under the project, when that cannot be told), opened without
//! an agent, with what was done in them.

use std::path::{Path, PathBuf};

/// A worktree an agent made, and what is in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Side {
    /// The task whose folder holds it; none for one in a temporary place.
    pub parent: Option<PathBuf>,
    /// Commits it has that the branch it came from has not.
    pub ahead: u32,
    /// Changes not committed.
    pub dirty: bool,
    /// What it was made for, as the agent that made it was asked.
    pub about: Option<String>,
}

/// Whether `path`, a worktree of the repository whose worktrees are `all`
/// (the main one first), is one an agent made: inside another worktree's
/// folder (that one is its parent), or in a temporary place.
pub fn made_by_agent(path: &Path, all: &[PathBuf]) -> Option<Option<PathBuf>> {
    // Where the app makes tasks, wherever the repository is.
    if let Some(main) = all.first() {
        let tasks = main.with_file_name(format!("{}.worktrees", main.file_name()?.to_string_lossy()));
        if path.starts_with(&tasks) && !path.components().any(|c| c.as_os_str() == ".claude") {
            return None;
        }
    }
    let parent = all.iter().filter(|other| other.as_path() != path && path.starts_with(other)).max_by_key(|other| other.components().count());
    if let Some(parent) = parent {
        return Some(Some(parent.clone()));
    }
    temporary(path).then_some(None)
}

/// What a Claude Code subagent's worktree (`.claude/worktrees/agent-<id>`)
/// is for: the description it was started with, which Claude Code keeps
/// next to the subagent's transcript, under the projects folder of the
/// session that started it (`from`, the worktree that session ran in).
pub fn about(path: &Path, from: &Path) -> Option<String> {
    let id = path.file_name()?.to_str()?.strip_prefix("agent-")?;
    let config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    let project = config.join("projects").join(project_dir(from));
    let file = format!("agent-{id}.meta.json");
    // A session folder each; the subagent's is in the one that started it.
    let meta = std::fs::read_dir(&project).ok()?.flatten().map(|session| session.path().join("subagents").join(&file)).find(|meta| meta.is_file())?;
    described(&std::fs::read_to_string(meta).ok()?)
}

/// Claude Code's folder name for a working directory: each character that
/// is not a letter or a digit becomes a dash.
fn project_dir(path: &Path) -> String {
    path.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn described(meta: &str) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(meta).ok()?;
    let description = meta["description"].as_str().map(str::trim).filter(|d| !d.is_empty())?;
    Some(description.chars().take(120).collect())
}

/// Under the system's temporary or cache folders, or Claude Code's own.
fn temporary(path: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut places = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    places.extend(std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from));
    if let Some(home) = &home {
        places.push(home.join(".cache"));
        places.push(home.join("Library/Caches"));
    }
    places.extend(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Temp")));
    let inside = |place: &PathBuf| path.starts_with(place) || place.canonicalize().is_ok_and(|real| path.starts_with(real));
    places.iter().any(inside) || path.components().collect::<Vec<_>>().windows(2).any(|w| w[0].as_os_str() == ".claude" && w[1].as_os_str() == "worktrees")
}

#[cfg(test)]
mod tests {
    use super::{described, made_by_agent, project_dir};
    use std::path::PathBuf;

    #[test]
    fn inside_a_task_or_in_a_temporary_place() {
        let p = PathBuf::from;
        let all = [p("/w/repo"), p("/w/repo.worktrees/DEV-1_x"), p("/w/repo/.claude/worktrees/agent-1"), p("/w/repo.worktrees/DEV-1_x/.claude/worktrees/agent-2")];
        assert_eq!(made_by_agent(&all[1], &all), None, "a task of the user's");
        assert_eq!(made_by_agent(&all[2], &all), Some(Some(p("/w/repo"))));
        assert_eq!(made_by_agent(&all[3], &all), Some(Some(p("/w/repo.worktrees/DEV-1_x"))), "the nearest");
        let tmp = std::env::temp_dir().join("try-1");
        assert_eq!(made_by_agent(&tmp, &all), Some(None));
        // A repository in a temporary place: its tasks are still tasks.
        let in_tmp = [std::env::temp_dir().join("r"), std::env::temp_dir().join("r.worktrees/DEV-2_y")];
        assert_eq!(made_by_agent(&in_tmp[1], &in_tmp), None);
        assert_eq!(made_by_agent(&p("/elsewhere/.claude/worktrees/agent-3"), &all), Some(None));
    }

    #[test]
    fn a_subagent_tells_what_it_is_for() {
        assert_eq!(project_dir(std::path::Path::new("/w/my.repo_x")), "-w-my-repo-x");
        let meta = r#"{"agentType":"general-purpose","worktreePath":"/w/repo/.claude/worktrees/agent-1","description":"Fix the login form","name":"login"}"#;
        assert_eq!(described(meta).as_deref(), Some("Fix the login form"));
        assert_eq!(described(r#"{"description":"  "}"#), None);
        assert_eq!(described("not json"), None);
    }
}
