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
}

/// Whether `path`, a worktree of the repository whose worktrees are `all`
/// (the main one first), is one an agent made: inside another worktree's
/// folder (that one is its parent), or in a temporary place.
pub fn made_by_agent(path: &Path, all: &[PathBuf]) -> Option<Option<PathBuf>> {
    let parent = all.iter().filter(|other| other.as_path() != path && path.starts_with(other)).max_by_key(|other| other.components().count());
    if let Some(parent) = parent {
        return Some(Some(parent.clone()));
    }
    temporary(path).then_some(None)
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
    use super::made_by_agent;
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
        assert_eq!(made_by_agent(&p("/elsewhere/.claude/worktrees/agent-3"), &all), Some(None));
    }
}
